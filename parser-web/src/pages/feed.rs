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
    fn grid_class(self) -> &'static str {
        match self {
            GridType::Auto => {
                "grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-3"
            }
            GridType::Three => "grid grid-cols-2 sm:grid-cols-3 gap-3",
            GridType::Two => "grid grid-cols-2 gap-3",
            GridType::One => "grid grid-cols-1 gap-3",
        }
    }

    /// How many skeleton cards to show for this grid type.
    pub fn skeleton_count(&self) -> usize {
        match self {
            GridType::Auto => 10,
            GridType::Three => 9,
            GridType::Two => 8,
            GridType::One => 4,
        }
    }
}

/// Read a display setting from unified settings_show_* key, falling back
/// to an old per-page key for backward compatibility.
pub fn read_display_setting(suffix: &str, old: &str, default: bool) -> bool {
    let new_key = format!("settings_show_{}", suffix);
    let storage = || window().and_then(|w| w.local_storage().ok().flatten());
    storage()
        .and_then(|s| s.get_item(&new_key).ok().flatten())
        .or_else(|| storage().and_then(|s| s.get_item(old).ok().flatten()))
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(default)
}

#[function_component(FeedPage)]
pub fn feed_page() -> Html {
    let _settings_tick = use_settings_tick();
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
    let show_desc = read_display_setting("desc", "hide_post_desc", false);
    let show_metadata = read_display_setting("metadata", "show_file_metadata", false);
    let show_breakdown = read_display_setting("breakdown", "show_score_breakdown", false);
    let show_detailed_breakdown =
        read_display_setting("detailed_breakdown", "show_score_detailed", false);
    // Per-page bottom-cutoff in percent (0..=95). 0 = show everything,
    // 30 = drop the bottom 30% of each fetched page by raw score, 95 =
    // keep only the top 5%. Decoupled from the model's raw `score` so
    // future scoring changes don't shift what the slider means.
    let cutoff_pct = use_state(|| {
        let storage = || window().and_then(|w| w.local_storage().ok().flatten());
        storage()
            .and_then(|s| s.get_item("settings_score_cutoff_pct").ok().flatten())
            .or_else(|| storage().and_then(|s| s.get_item("page_cutoff_pct").ok().flatten()))
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(0.0)
            .clamp(0.0, 95.0)
    });
    let grid = {
        let storage = || window().and_then(|w| w.local_storage().ok().flatten());
        let stored = storage()
            .and_then(|s| s.get_item("settings_grid_type").ok().flatten())
            .or_else(|| storage().and_then(|s| s.get_item("feed_grid_type").ok().flatten()));
        GridType::from_storage(stored)
    };

    // Badge visibility toggles — read from unified settings_show_* keys,
    // falling back to the legacy `feed_show_*` keys.
    let show_rating = read_display_setting("rating", "feed_show_rating", true);
    let show_affinity = read_display_setting("affinity", "feed_show_affinity", true);
    let show_score = read_display_setting("score", "feed_show_score", true);
    let show_post_number = read_display_setting("post_number", "feed_show_post_number", true);

    // Exploration epsilon — ε-greedy exploration bonus.
    // 0.0 = pure exploitation (Focused), 0.5 = max exploration (Discovery).
    // Initialised from localStorage; falls back to 0.1 (Balanced).
    let exploration_epsilon = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("feed_exploration_epsilon").ok().flatten())
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(0.1)
            .clamp(0.0, 0.5)
    });

    {
        let cutoff_pct = cutoff_pct.clone();
        use_effect_with(*cutoff_pct, move |a: &f32| {
            if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = store.set_item("settings_score_cutoff_pct", &a.to_string());
            }
            || ()
        });
    }

    {
        let exploration_epsilon = exploration_epsilon.clone();
        use_effect_with(*exploration_epsilon, move |a: &f32| {
            if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = store.set_item("feed_exploration_epsilon", &a.to_string());
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
        let exploration_epsilon = exploration_epsilon.clone();
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
                "{}/recommendations/{}?page={}&session={}&exploration={:.2}",
                cfg.backend_domain,
                user.id,
                *page,
                urlencoding::encode(session_id.as_str()),
                *exploration_epsilon,
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
                            let mut sorted: Vec<f32> = new_items.iter().map(|p| p.score).collect();
                            sorted.sort_by(|a, b| {
                                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let frac = (cutoff_value / 100.0).clamp(0.0, 0.99);
                            let idx = ((sorted.len() as f32 - 1.0) * frac).round() as usize;
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

    // Changing the exploration preset resets scroll state so the next
    // scroll event fetches a fresh page with the new epsilon value.
    {
        let exhausted = exhausted.clone();
        let consecutive_empty = consecutive_empty.clone();
        let consecutive_errors = consecutive_errors.clone();
        let error = error.clone();
        use_effect_with(*exploration_epsilon, move |_| {
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
    let card_account_id = selected_user
        .as_ref()
        .map(|u| u.id as i32)
        .unwrap_or_default();
    let card_grid_class = grid.grid_class();
    let card_session_id = (*session_id).clone();
    let feed_cards = render_post_grid(
        &posts,
        card_grid_class,
        &backend_url,
        card_account_id,
        &card_session_id,
        1,
        show_rating,
        show_affinity,
        show_score,
        show_post_number,
        show_desc,
        show_metadata,
        show_breakdown,
        show_detailed_breakdown,
    );

    html! {
        <div class="m-4 gap-2 feed-page">
            <h1 class="text-2xl font-semibold text-base-content mb-3">{ "Latest Posts" }</h1>

            <div id="feed-account">
                <SavedAccountsSelect
                    selected_user={selected_user.clone()}
                    is_loading={is_loading.clone()}
                />
            </div>

            <div class="flex flex-wrap gap-3 items-center feed-toolbar">
                <div class="feed-exploration-col" id="feed-exploration">
                    <label for="feed-exploration-input" class="mb-1 block">
                        <span class="text-base-content">{"Exploration"}
                        <span class="text-xs text-base-content/70 ms-1">
                            { "(ε-greedy novelty boost)" }
                        </span></span>
                    </label>
                    <div class="flex items-center gap-2">
                        <input
                            id="feed-exploration-input"
                            type="number"
                            class="input input-bordered"
                            style="max-width: 8rem"
                            value={exploration_epsilon.to_string()}
                            step="0.05"
                            min="0"
                            max="0.5"
                            oninput={{
                                let exploration_epsilon = exploration_epsilon.clone();
                                Callback::from(move |e: InputEvent| {
                                    if let Some(target) = e.target()
                                        && let Ok(input) = target.dyn_into::<HtmlInputElement>()
                                            && let Ok(v) = input.value().parse::<f32>() {
                                                exploration_epsilon.set(v.clamp(0.0, 0.5));
                                            }
                                })
                            }}
                        />
                        <div class="join join-sm" role="group" aria-label="Exploration preset">
                            {
                                [
                                    ("Focused", 0.0f32, "Pure exploitation — show only the model's top picks. Same as default behaviour."),
                                    ("Balanced", 0.1f32, "Mild exploration — occasionally surface novel content alongside confident picks."),
                                    ("Discovery", 0.4f32, "Heavy exploration — prioritise novel and less-similar content for broader discovery."),
                                ].iter().map(|(label, value, tip)| {
                                    let val = *value;
                                    let active = (*exploration_epsilon - val).abs() < 0.025;
                                    let exploration_epsilon = exploration_epsilon.clone();
                                    html! {
                                        <button
                                            type="button"
                                            class={classes!("btn", "btn-outline", if active { Some("btn-active") } else { None })}
                                            title={ *tip }
                                            aria-pressed={active.to_string()}
                                            onclick={Callback::from(move |_| exploration_epsilon.set(val))}
                                        >
                                            { *label }
                                        </button>
                                    }
                                }).collect::<Html>()
                            }
                        </div>
                    </div>
                </div>

        </div>

            <div class="fixed bottom-0 left-1/2 -translate-x-1/2 w-full flex justify-between z-1 feed-statusbar">
                {
                    if let Some(u) = &*selected_user {
                        html! { <span class="text-sm m-3 bg-base-200 bg-opacity-75 text-base-content rounded-full badge shadow-sm">{ format!("User: {} (ID: {})", u.name, u.id) }</span> }
                    } else {
                        html! { <span class="text-sm m-3 bg-base-200 bg-opacity-75 text-base-content rounded-full badge shadow-sm">{ "No user selected" }</span> }
                    }
                }
                {
                    if !(*is_loading) && (*error).is_none() {
                        let label = if *exhausted {
                            format!("Loaded {} posts — nothing left to fetch above the per-page cutoff", posts.len())
                        } else {
                            format!("Loaded {} posts", posts.len())
                        };
                        html! { <span class="text-sm m-3 bg-base-200 bg-opacity-75 text-base-content rounded-full badge shadow-sm" aria-live="polite">{ label }</span> }
                    } else {
                        let loading_msg = format!("Loading... ({} posts loaded)", posts.len());
                        html!{ <span class="text-sm m-3 bg-base-200 bg-opacity-75 text-base-content rounded-full badge shadow-sm" aria-live="polite"><span class="loading loading-spinner loading-sm me-1" role="status"></span>{ loading_msg }</span>}
                    }
                }
            </div>

            {
                if let Some(err) = &*error {
                    html! {
                        <div class="alert alert-error flex justify-between items-center" role="alert" aria-live="polite">
                            <span>{ err }</span>
                            <button
                                class="btn btn-sm btn-outline"
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
                                        if *exhausted {
                                            consecutive_errors.borrow().set(0);
                                        }
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
                        <div class="text-center text-base-content/70 my-5" aria-live="polite">
                            { "No posts yet." }
                        </div>
                    }
                } else { html!{} }
            }

            <div class="feed-grid" aria-busy={(*is_loading).to_string()}>
                { feed_cards }
            </div>

            {
                if *is_loading && posts.is_empty() {
                    let count = grid.skeleton_count();
                    let skeleton_card = |_| html! {
                        <div class="card bg-base-100 shadow-sm">
                            <div class="skeleton w-full" style="aspect-ratio: 1 / 1; border-radius: 0;"></div>
                            <div class="card-body gap-2">
                                <div class="skeleton h-4 w-3/4"></div>
                                <div class="skeleton h-3 w-1/2"></div>
                                <div class="skeleton h-3 w-2/3"></div>
                            </div>
                        </div>
                    };
                    html! {
                        <div class={card_grid_class.to_string() + " m-3 feed-grid"}>
                            { for (0..count).map(skeleton_card) }
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
        .map_err(|e| humanize_network_error(format!("{e:?}")))?;

    let resp: Response = resp_value
        .dyn_into()
        .map_err(|_| "Failed to cast Response".to_string())?;

    let text_promise = resp
        .text()
        .map_err(|e| format!("Failed to read response text: {e:?}"))?;
    let text_js = wasm_bindgen_futures::JsFuture::from(text_promise)
        .await
        .map_err(|e| humanize_network_error(format!("{e:?}")))?;
    let text = text_js
        .as_string()
        .ok_or("Response text not a string".to_string())?;

    if !resp.ok() {
        return Err(humanize_error_body(resp.status(), &text));
    }

    serde_json::from_str::<T>(&text).map_err(|e| format!("JSON parse error: {e}"))
}
