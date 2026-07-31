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
    let show_detailed_breakdown = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("show_score_detailed").ok().flatten())
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

    // Badge visibility toggles — each defaults to enabled (true) and
    // persists to localStorage under `feed_show_*` keys so the user's
    // preference survives reloads.
    let show_rating = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("feed_show_rating").ok().flatten())
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(true)
    });
    let show_affinity = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("feed_show_affinity").ok().flatten())
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(true)
    });
    let show_score = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("feed_show_score").ok().flatten())
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(true)
    });
    let show_post_number = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("feed_show_post_number").ok().flatten())
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(true)
    });

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
        {
            let a = *show_detailed_breakdown;
            use_effect_with(a, move |a| {
                if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                    let _ = store.set_item("show_score_detailed", &a.to_string());
                }
                || ()
            });
        }
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

    // Persist badge visibility toggles.
    macro_rules! persist_bool {
        ($s:expr, $key:expr) => {{
            let s = $s.clone();
            let key = $key;
            use_effect_with(*s, move |a: &bool| {
                if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                    let _ = store.set_item(key, &a.to_string());
                }
                || ()
            });
        }};
    }
    persist_bool!(show_rating, "feed_show_rating");
    persist_bool!(show_affinity, "feed_show_affinity");
    persist_bool!(show_score, "feed_show_score");
    persist_bool!(show_post_number, "feed_show_post_number");

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
    let card_grid_class = (*grid).grid_class();
    let card_session_id = (*session_id).clone();
    let feed_cards = render_post_grid(
        &posts,
        card_grid_class,
        &backend_url,
        card_account_id,
        &card_session_id,
        1,
        *show_rating,
        *show_affinity,
        *show_score,
        *show_post_number,
        *show_desc,
        *show_metadata,
        *show_breakdown,
        *show_detailed_breakdown,
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
                <div class="feed-affinity-col" id="feed-affinity">
                    <label for="feed-affinity-input" class="mb-1 block">
                        <span class="text-base-content">{"Per-page cutoff"}
                        <span class="text-xs text-base-content/70 ms-1">
                            { "(% of worst posts to drop)" }
                        </span></span>
                    </label>
                    <div class="flex items-center gap-2">
                        <input
                            id="feed-affinity-input"
                            type="number"
                            class="input input-bordered"
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
                        <div class="join join-sm" role="group" aria-label="Cutoff preset">
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
                                            class={classes!("btn", "btn-outline", if active { Some("btn-active") } else { None })}
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

                <div class="feed-grid-col" id="feed-grid">
                    <span class="block">{"Grid type"}</span>
                    <div class="join" role="group" aria-label="Grid type">
                        <button
                            type="button"
                            class={classes!("btn", "btn-outline", if *grid == GridType::Auto { "btn-active" } else { "" })}
                            aria-pressed={(*grid == GridType::Auto).to_string()}
                            aria-label="Auto grid (responsive)"
                            title="Auto grid (responsive)"
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::Auto))
                            }}
                        >
                            <IconWater />
                        </button>

                        <button
                            type="button"
                            class={classes!("btn", "btn-outline", if *grid == GridType::Three { "btn-active" } else { "" })}
                            aria-pressed={(*grid == GridType::Three).to_string()}
                            aria-label="Three-column grid"
                            title="Three-column grid"
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::Three))
                            }}
                        >
                            <IconGrid3x3 />
                        </button>

                        <button
                            type="button"
                            class={classes!("btn", "btn-outline", if *grid == GridType::Two { "btn-active" } else { "" })}
                            aria-pressed={(*grid == GridType::Two).to_string()}
                            aria-label="Two-column grid"
                            title="Two-column grid"
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::Two))
                            }}
                        >
                            <IconGridFill />
                        </button>

                        <button
                            type="button"
                            class={classes!("btn", "btn-outline", if *grid == GridType::One { "btn-active" } else { "" })}
                            aria-pressed={(*grid == GridType::One).to_string()}
                            aria-label="Single-column list"
                            title="Single-column list"
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::One))
                            }}
                        >
                            <IconSquareFill />
                        </button>
                    </div>
                </div>

                <div class="self-end">
                    <details class="dropdown dropdown-end">
                        <summary class="btn btn-outline">
                            <IconSliders />
                            {" Display"}
                        </summary>
                        <div class="menu dropdown-content p-3 shadow bg-base-100 rounded-box w-72 z-50" style="min-width: 260px;">
                            <span class="text-xs text-base-content/70 block mb-1">{ "Badges" }</span>
                            <label class="label cursor-pointer py-1">
                                <span class="text-base-content">{"Rating badge"}</span>
                                <input
                                    type="checkbox"
                                    class="toggle toggle-sm"
                                    checked={*show_rating}
                                    onchange={{
                                        let show_rating = show_rating.clone();
                                        Callback::from(move |_: Event| show_rating.set(!*show_rating))
                                    }}
                                />
                            </label>
                            <label class="label cursor-pointer py-1">
                                <span class="text-base-content">{"Affinity score"}</span>
                                <input
                                    type="checkbox"
                                    class="toggle toggle-sm"
                                    checked={*show_affinity}
                                    onchange={{
                                        let show_affinity = show_affinity.clone();
                                        Callback::from(move |_: Event| show_affinity.set(!*show_affinity))
                                    }}
                                />
                            </label>
                            <label class="label cursor-pointer py-1">
                                <span class="text-base-content">{"Post score"}</span>
                                <input
                                    type="checkbox"
                                    class="toggle toggle-sm"
                                    checked={*show_score}
                                    onchange={{
                                        let show_score = show_score.clone();
                                        Callback::from(move |_: Event| show_score.set(!*show_score))
                                    }}
                                />
                            </label>
                            <label class="label cursor-pointer py-1">
                                <span class="text-base-content">{"Post number"}</span>
                                <input
                                    type="checkbox"
                                    class="toggle toggle-sm"
                                    checked={*show_post_number}
                                    onchange={{
                                        let show_post_number = show_post_number.clone();
                                        Callback::from(move |_: Event| show_post_number.set(!*show_post_number))
                                    }}
                                />
                            </label>
                            <div class="divider my-1"></div>
                            <span class="text-xs text-base-content/70 block mb-1">{ "Cards" }</span>
                            <label class="label cursor-pointer py-1">
                                <span class="text-base-content">{"Post text / tags"}</span>
                                <input
                                    type="checkbox"
                                    class="toggle toggle-sm"
                                    checked={*show_desc}
                                    onchange={{
                                        let show_desc = show_desc.clone();
                                        Callback::from(move |_: Event| show_desc.set(!*show_desc))
                                    }}
                                />
                            </label>
                            <label class="label cursor-pointer py-1">
                                <span class="text-base-content">{"File metadata"}</span>
                                <input
                                    type="checkbox"
                                    class="toggle toggle-sm"
                                    checked={*show_metadata}
                                    onchange={{
                                        let show_metadata = show_metadata.clone();
                                        Callback::from(move |_: Event| show_metadata.set(!*show_metadata))
                                    }}
                                />
                            </label>
                            <label class="label cursor-pointer py-1">
                                <span class="text-base-content">{"Score breakdown"}</span>
                                <input
                                    type="checkbox"
                                    class="toggle toggle-sm"
                                    checked={*show_breakdown}
                                    onchange={{
                                        let show_breakdown = show_breakdown.clone();
                                        Callback::from(move |_: Event| show_breakdown.set(!*show_breakdown))
                                    }}
                                />
                            </label>
                            <label class="label cursor-pointer py-1">
                                <span class="text-base-content">{"Detailed"}</span>
                                <input
                                    type="checkbox"
                                    class="toggle toggle-sm"
                                    checked={*show_detailed_breakdown}
                                    onchange={{
                                        let show_detailed_breakdown = show_detailed_breakdown.clone();
                                        Callback::from(move |_: Event| show_detailed_breakdown.set(!*show_detailed_breakdown))
                                    }}
                                />
                            </label>
                        </div>
                    </details>

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
                    let count = (*grid).skeleton_count();
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
