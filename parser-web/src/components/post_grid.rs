use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::js_sys;
use yew::prelude::*;

use crate::components::{ErrorAlert, PostCard};
use crate::models::*;

/// Determine the current number of columns from a `columns-*` / `grid-cols-*`
/// class string by matching the viewport width against Tailwind breakpoints.
/// Best-known pixel dimensions for layout calculations.
/// Prefers preview > sample > original, falling back to (4, 3) for 4:3.
pub(crate) fn best_dimensions(files: &crate::models::Files) -> (i64, i64) {
    let w = files.preview.width.max(1);
    let h = files.preview.height.max(1);
    if w > 1 && h > 1 {
        return (w, h);
    }
    let w = files.sample.width.max(1);
    let h = files.sample.height.max(1);
    if w > 1 && h > 1 {
        return (w, h);
    }
    let w = files.original.width.max(1);
    let h = files.original.height.max(1);
    if w > 1 && h > 1 { (w, h) } else { (4, 3) }
}

fn current_column_count(grid_class: &str) -> usize {
    // Determine which Tailwind breakpoints this class references and their
    // corresponding column counts.
    let width = web_sys::window()
        .and_then(|w| w.inner_width().ok())
        .and_then(|w| w.as_f64())
        .unwrap_or(1024.0);

    // Tailwind breakpoint min-widths.
    // sm=640, md=768, lg=1024, xl=1280
    let mut cols_by_bp: Vec<(f64, usize)> = Vec::new();
    for part in grid_class.split_whitespace() {
        let parsed = if let Some(c) = part
            .strip_prefix("xl:grid-cols-")
            .or_else(|| part.strip_prefix("xl:columns-"))
        {
            c.parse::<usize>().ok().map(|n| (1280.0, n))
        } else if let Some(c) = part
            .strip_prefix("lg:grid-cols-")
            .or_else(|| part.strip_prefix("lg:columns-"))
        {
            c.parse::<usize>().ok().map(|n| (1024.0, n))
        } else if let Some(c) = part
            .strip_prefix("md:grid-cols-")
            .or_else(|| part.strip_prefix("md:columns-"))
        {
            c.parse::<usize>().ok().map(|n| (768.0, n))
        } else if let Some(c) = part
            .strip_prefix("sm:grid-cols-")
            .or_else(|| part.strip_prefix("sm:columns-"))
        {
            c.parse::<usize>().ok().map(|n| (640.0, n))
        } else if let Some(c) = part
            .strip_prefix("grid-cols-")
            .or_else(|| part.strip_prefix("columns-"))
        {
            c.parse::<usize>().ok().map(|n| (0.0, n))
        } else {
            None
        };
        if let Some((bp, cols)) = parsed {
            cols_by_bp.push((bp, cols));
        }
    }

    cols_by_bp.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    // The baseline default (no breakpoint prefix) applies below sm.
    let mut result = cols_by_bp.first().map(|(_, n)| *n).unwrap_or(1);
    for (bp, n) in &cols_by_bp {
        if width >= *bp {
            result = *n;
        }
    }
    result
}

/// Skeleton placeholder that mirrors the visual shape of a `PostCard`:
/// media area (4:3 reserve) plus a few text lines. Used while a grid is
/// loading its first page so the layout doesn't collapse to a spinner.
#[function_component(SkeletonCard)]
pub fn skeleton_card() -> Html {
    html! {
        <div class="card post-card card-compact overflow-hidden w-full relative border border-base-300 shadow-sm break-inside-avoid mb-3">
            <div class="skeleton w-full" style="aspect-ratio: 4 / 3; border-radius: 0;"></div>
            <div class="p-2 space-y-2">
                <div class="skeleton h-4 w-3/4"></div>
                <div class="skeleton h-3 w-1/2"></div>
                <div class="skeleton h-3 w-2/3"></div>
            </div>
        </div>
    }
}

/// Render a grid of skeleton cards for the given grid layout, matching the
/// column count the real post grid will use. Call while the first page is
/// loading so the page height is reserved and the layout stays stable.
pub fn render_post_grid_skeleton(grid_class: &str) -> Html {
    let effective_class = if grid_class.is_empty() {
        "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5 xl:grid-cols-6 gap-3"
    } else {
        grid_class
    };
    let num_columns = current_column_count(effective_class).max(1);
    html! {
        <div class={format!("{} m-3", effective_class)} style="align-items: start;" aria-hidden="true">
            { for (0..num_columns).map(|_| html! {
                <div class="flex flex-col">
                    { for (0..3).map(|_| html! { <SkeletonCard /> }) }
                </div>
            }) }
        </div>
    }
}

/// Render the shared masonry-style post layout used by all post collections.
#[allow(
    clippy::too_many_arguments,
    reason = "The shared renderer accepts display settings independently so pages can persist and choose each option."
)]
pub fn render_post_grid(
    posts: &[ScoredPost],
    grid_class: &str,
    backend_url: &str,
    account_id: i32,
    session_id: &str,
    position_offset: usize,
    show_rating: bool,
    show_affinity: bool,
    show_score: bool,
    show_post_number: bool,
    show_desc: bool,
    show_metadata: bool,
    show_breakdown: bool,
    show_detailed_breakdown: bool,
) -> Html {
    if posts.is_empty() {
        return html! {};
    }

    let effective_class = if grid_class.is_empty() {
        "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5 xl:grid-cols-6 gap-3"
    } else {
        grid_class
    };
    let num_columns = current_column_count(effective_class).max(1);
    let mut col_heights = vec![0.0f64; num_columns];
    let mut columns: Vec<Vec<(usize, &ScoredPost)>> =
        (0..num_columns).map(|_| Vec::new()).collect();

    for (post_index, post) in posts.iter().enumerate() {
        // Use preview dimensions as the best proxy for the rendered card's
        // aspect ratio — they always reflect the actual image proportions
        // even when original/sample sizes are 0x0 (e.g. deleted posts with
        // fallback preview URLs that have valid dimensions).
        let (w, h) = best_dimensions(&post.post.files);
        let width = w.max(1) as f64;
        let height = h.max(1) as f64;
        let aspect = height / width;
        let shortest = col_heights
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(index, _)| index)
            .unwrap_or(0);
        col_heights[shortest] += aspect;
        columns[shortest].push((post_index, post));
    }

    html! {
        <div class={format!("{} m-3", effective_class)} style="align-items: start;">
            { for columns.into_iter().enumerate().map(|(column_index, column_posts)| html! {
                <div key={column_index} class="flex flex-col">
                    { for column_posts.into_iter().map(|(post_index, scored_post)| html! {
                        <PostCard
                            key={scored_post.post.id}
                            post={Rc::new(scored_post.post.clone())}
                            affinity={scored_post.score}
                            backend_url={backend_url.to_string()}
                            account_id={account_id}
                            session_id={session_id.to_string()}
                            position={(position_offset + post_index) as i32}
                            breakdown={scored_post.breakdown.clone()}
                            show_rating={show_rating}
                            show_affinity={show_affinity}
                            show_score={show_score}
                            show_post_number={show_post_number}
                            show_desc={show_desc}
                            show_metadata={show_metadata}
                            show_breakdown={show_breakdown}
                            show_detailed_breakdown={show_detailed_breakdown}
                        />
                    }) }
                </div>
            }) }
        </div>
    }
}

/// Reusable post grid with infinite scroll, status bar, and post-card display.
#[derive(Properties, PartialEq)]
pub struct PostGridProps {
    /// Base URL for fetching posts (page param is appended automatically).
    pub fetch_url: String,
    /// Whether the response is `Vec<ScoredPost>` (true) or `Vec<Post>` (false).
    /// When false, posts get a score of 0.0 and no breakdown.
    pub scored: bool,
    /// Display toggles — plain bool, wired from parent state via deref
    /// so the grid re-renders when the parent re-renders.
    #[prop_or(true)]
    pub show_rating: bool,
    #[prop_or(false)]
    pub show_affinity: bool,
    #[prop_or(true)]
    pub show_score: bool,
    #[prop_or(true)]
    pub show_post_number: bool,
    #[prop_or(true)]
    pub show_desc: bool,
    #[prop_or(false)]
    pub show_metadata: bool,
    #[prop_or(false)]
    pub show_breakdown: bool,
    #[prop_or(false)]
    pub show_detailed_breakdown: bool,
    /// Optional per-page percentage of the lowest scored results to omit.
    #[prop_or_default]
    pub score_cutoff_pct: Option<f32>,
    /// Empty-state message when no user is selected.
    pub empty_message: String,
    /// Grid layout class (overrides default responsive grid).
    #[prop_or_default]
    pub grid_class: String,
    /// Context forwarded to PostCard for interactions and account-scoped state.
    #[prop_or_default]
    pub backend_url: String,
    #[prop_or_default]
    pub account_id: i32,
}

#[function_component(PostGrid)]
pub fn post_grid(props: &PostGridProps) -> Html {
    let posts = use_state(Vec::<ScoredPost>::new);
    let page = use_state(|| 0usize);
    let is_loading = use_state(|| false);
    let exhausted = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    let scroll_sentinel = use_node_ref();
    // Random session token for dedup across pages.
    let session_id = use_state(|| {
        format!(
            "browse-{}-{}",
            js_sys::Date::now() as u64,
            (js_sys::Math::random() * 1_000_000_000.0) as u64
        )
    });

    // Fetch trigger — bumped when URL changes to start a fresh fetch.
    let fetch_trigger: UseStateHandle<u32> = use_state(|| 0);

    // Reset + trigger first fetch when the source or score cutoff changes.
    {
        let url = (props.fetch_url.clone(), props.score_cutoff_pct);
        let posts = posts.clone();
        let page = page.clone();
        let exhausted = exhausted.clone();
        let ft = fetch_trigger.clone();
        use_effect_with(url, move |_| {
            posts.set(Vec::new());
            page.set(0);
            exhausted.set(false);
            ft.set(*ft + 1);
            || ()
        });
    }

    // Fetch more posts.
    let fetch_more = {
        let url = props.fetch_url.clone();
        let posts = posts.clone();
        let page = page.clone();
        let is_loading = is_loading.clone();
        let exhausted = exhausted.clone();
        let error = error.clone();
        let scored = props.scored;
        let score_cutoff_pct = props.score_cutoff_pct;
        let session_id = session_id.clone();
        Callback::from(move |_| {
            if *is_loading || *exhausted {
                return;
            }
            is_loading.set(true);
            error.set(None);
            let next = *page + 1;
            let sep = if url.contains('?') { "&" } else { "?" };
            let page_url = format!("{}{sep}page={}", url, next);
            let _session = session_id.clone();
            let scored = scored;
            let posts_cb = posts.clone();
            let page_cb = page.clone();
            let is_loading_cb = is_loading.clone();
            let exhausted_cb = exhausted.clone();
            let error_cb = error.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match api_get(&page_url).send().await {
                    Ok(resp) if resp.ok() => {
                        let raw = resp.text().await.unwrap_or_default();
                        let mut new_items: Vec<ScoredPost> = if scored {
                            serde_json::from_str::<Vec<ScoredPost>>(&raw).unwrap_or_default()
                        } else {
                            let posts: Vec<Post> = serde_json::from_str(&raw).unwrap_or_default();
                            posts
                                .into_iter()
                                .map(|p| ScoredPost {
                                    post: p,
                                    score: 0.0,
                                    breakdown: None,
                                })
                                .collect()
                        };
                        if let Some(cutoff) = score_cutoff_pct
                            && scored
                            && new_items.len() >= 2
                        {
                            let mut scores: Vec<f32> =
                                new_items.iter().map(|item| item.score).collect();
                            scores.sort_by(|a, b| {
                                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let index = ((scores.len() - 1) as f32
                                * (cutoff / 100.0).clamp(0.0, 0.95))
                                as usize;
                            let threshold = scores[index];
                            new_items.retain(|item| item.score >= threshold);
                        }
                        // Dedup against already-loaded posts.
                        let mut merged = (*posts_cb).clone();
                        let mut seen: std::collections::HashSet<i64> =
                            merged.iter().map(|p| p.post.id).collect();
                        new_items.retain(|p| seen.insert(p.post.id));
                        if new_items.is_empty() {
                            exhausted_cb.set(true);
                        } else {
                            merged.extend(new_items);
                            posts_cb.set(merged);
                            page_cb.set(next);
                        }
                        is_loading_cb.set(false);
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        error_cb.set(Some(humanize_error_body(status, &body)));
                        is_loading_cb.set(false);
                    }
                    Err(err) => {
                        error_cb.set(Some(humanize_network_error(err)));
                        is_loading_cb.set(false);
                    }
                }
            });
        })
    };

    // Initial fetch when fetch_trigger changes (URL loaded).
    {
        let fetch_more = fetch_more.clone();
        let initial = *fetch_trigger;
        use_effect_with(initial, move |_| {
            if initial > 0 {
                fetch_more.emit(());
            }
            || ()
        });
    }

    // IntersectionObserver for infinite scroll (subsequent pages).
    // Depend on posts length so the observer re-attaches once the sentinel
    // element is actually in the DOM (after the first page loads).
    {
        let fetch_more = fetch_more.clone();
        let sentinel = scroll_sentinel.clone();
        let posts_len = (*posts).len();
        use_effect_with(posts_len, move |_| {
            let fetch = fetch_more.clone();
            let cb = Closure::<dyn FnMut(Vec<web_sys::IntersectionObserverEntry>)>::new(
                move |entries: Vec<web_sys::IntersectionObserverEntry>| {
                    if let Some(entry) = entries.first()
                        && entry.is_intersecting()
                    {
                        fetch.emit(());
                    }
                },
            );
            let observer = web_sys::IntersectionObserver::new(cb.as_ref().unchecked_ref()).ok();
            if let Some(o) = observer
                && let Some(el) = sentinel.cast::<web_sys::Element>()
            {
                o.observe(&el);
                cb.forget();
                std::mem::forget(o);
            }
            || ()
        });
    }

    let grid_class = if props.grid_class.is_empty() {
        "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5 xl:grid-cols-6 gap-3"
    } else {
        &props.grid_class
    };

    html! {
        <div>
            {
                if (*posts).is_empty() && *is_loading && error.is_none() {
                    // First page still loading: skeleton cards instead of a spinner.
                    html! { render_post_grid_skeleton(grid_class) }
                } else if (*posts).is_empty() && !*is_loading && error.is_none() {
                    html! { <p class="text-base-content/70 text-center my-8">{ &props.empty_message }</p> }
                } else {
                    html! {
                        <>
                            {
                                if let Some(error) = &*error {
                                    html! { <ErrorAlert message={error.clone()} on_retry={Some(fetch_more.clone())} /> }
                                } else { html! {} }
                            }
                            { render_post_grid(
                                &posts,
                                grid_class,
                                &props.backend_url,
                                props.account_id,
                                &session_id,
                                0,
                                props.show_rating,
                                props.show_affinity,
                                props.show_score,
                                props.show_post_number,
                                props.show_desc,
                                props.show_metadata,
                                props.show_breakdown,
                                props.show_detailed_breakdown,
                            ) }
                            <div ref={scroll_sentinel} class="h-4"></div>
                            {
                                if *is_loading {
                                    html! { <div class="flex justify-center my-4"><span class="loading loading-spinner loading-lg" role="status"></span></div> }
                                } else { html!{} }
                            }
                        </>
                    }
                }
            }
        </div>
    }
}
