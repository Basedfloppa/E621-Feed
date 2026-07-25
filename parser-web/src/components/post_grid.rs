use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::js_sys;
use yew::prelude::*;

use crate::components::PostCard;
use crate::models::*;

/// Determine the current number of columns from a `columns-*` / `grid-cols-*`
/// class string by matching the viewport width against Tailwind breakpoints.
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

/// Reusable post grid with infinite scroll, status bar, and post-card display.
#[derive(Properties, PartialEq)]
pub struct PostGridProps {
    /// Base URL for fetching posts (page param is appended automatically).
    pub fetch_url: String,
    /// Whether the response is `Vec<ScoredPost>` (true) or `Vec<Post>` (false).
    /// When false, posts get a score of 0.0 and no breakdown.
    pub scored: bool,
    /// Display toggles
    pub show_rating: UseStateHandle<bool>,
    pub show_affinity: UseStateHandle<bool>,
    pub show_score: UseStateHandle<bool>,
    pub show_post_number: UseStateHandle<bool>,
    pub show_desc: UseStateHandle<bool>,
    pub show_metadata: UseStateHandle<bool>,
    pub show_breakdown: UseStateHandle<bool>,
    /// Empty-state message when no user is selected.
    pub empty_message: String,
    /// Grid layout class (overrides default responsive grid).
    #[prop_or_default]
    pub grid_class: String,
}

#[function_component(PostGrid)]
pub fn post_grid(props: &PostGridProps) -> Html {
    let posts = use_state(Vec::<ScoredPost>::new);
    let page = use_state(|| 0usize);
    let is_loading = use_state(|| false);
    let exhausted = use_state(|| false);
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

    // Reset + trigger first fetch when fetch_url changes.
    {
        let url = props.fetch_url.clone();
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
        let scored = props.scored;
        let session_id = session_id.clone();
        Callback::from(move |_| {
            if *is_loading || *exhausted { return; }
            is_loading.set(true);
            let next = *page + 1;
            let sep = if url.contains('?') { "&" } else { "?" };
            let page_url = format!("{}{sep}page={}", url, next);
            let _session = session_id.clone();
            let scored = scored;
            let posts_cb = posts.clone();
            let page_cb = page.clone();
            let is_loading_cb = is_loading.clone();
            let exhausted_cb = exhausted.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match api_get(&page_url).send().await {
                    Ok(resp) if resp.ok() => {
                        let raw = resp.text().await.unwrap_or_default();
                        let mut new_items: Vec<ScoredPost> = if scored {
                            serde_json::from_str::<Vec<ScoredPost>>(&raw)
                                .unwrap_or_default()
                        } else {
                            let posts: Vec<Post> = serde_json::from_str(&raw)
                                .unwrap_or_default();
                            posts.into_iter()
                                .map(|p| ScoredPost {
                                    post: p,
                                    score: 0.0,
                                    breakdown: None,
                                })
                                .collect()
                        };
                        // Dedup against already-loaded posts.
                        let mut merged = (*posts_cb).clone();
                        let mut seen: std::collections::HashSet<i64>
                            = merged.iter().map(|p| p.post.id).collect();
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
                    Ok(_) => { is_loading_cb.set(false); }
                    Err(_) => { is_loading_cb.set(false); }
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
    {
        let fetch_more = fetch_more.clone();
        let sentinel = scroll_sentinel.clone();
        use_effect_with((), move |_| {
            let fetch = fetch_more.clone();
            let cb = Closure::<dyn FnMut(Vec<web_sys::IntersectionObserverEntry>)>::new(
                move |entries: Vec<web_sys::IntersectionObserverEntry>| {
                    if let Some(entry) = entries.first()
                        && entry.is_intersecting() {
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
                let _ = o;
            }
            || ()
        });
    }

    // Determine the effective number of columns from viewport width + grid class.
    let num_columns = {
        let cls = if props.grid_class.is_empty() {
            "grid-cols-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5 xl:grid-cols-6 gap-3"
        } else {
            &props.grid_class
        };
        current_column_count(cls)
    };

    // Outer CSS Grid with responsive column layout.
    let grid_class = if props.grid_class.is_empty() {
        "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5 xl:grid-cols-6 gap-3"
    } else {
        &props.grid_class
    };

    // Split posts into N columns (round-robin) so that column 1 gets posts
    // 0, N, 2N…, column 2 gets posts 1, N+1, 2N+1… etc.  This preserves
    // row-major ordering: top of column 1 = post 0, top of column 2 = post 1.
    let columns: Vec<Vec<ScoredPost>> = {
        let n = num_columns.max(1);
        let mut cols: Vec<Vec<ScoredPost>> = (0..n).map(|_| Vec::new()).collect();
        for (i, sp) in (*posts).iter().enumerate() {
            cols[i % n].push(sp.clone());
        }
        cols
    };

    html! {
        <div>
            {
                if (*posts).is_empty() && !*is_loading {
                    html! { <p class="text-base-content/70 text-center my-8">{ &props.empty_message }</p> }
                } else {
                    html! {
                        <>
                            <div class={format!("{} m-3", grid_class)} style="align-items: start;">
                                { for columns.iter().map(|col_posts| html! {
                                    <div class="flex flex-col">
                                        { for col_posts.iter().map(|sp| html! {
                                            <PostCard
                                                post={Rc::new(sp.post.clone())}
                                                affinity={sp.score}
                                                backend_url={""}
                                                account_id={0}
                                                session_id={(*session_id).clone()}
                                                position={0}
                                                breakdown={sp.breakdown.clone()}
                                                show_rating={*props.show_rating}
                                                show_affinity={*props.show_affinity}
                                                show_score={*props.show_score}
                                                show_post_number={*props.show_post_number}
                                                show_desc={*props.show_desc}
                                                show_metadata={*props.show_metadata}
                                                show_breakdown={*props.show_breakdown}
                                            />
                                        }) }
                                    </div>
                                }) }
                            </div>
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
