use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::js_sys;
use yew::prelude::*;

use crate::components::{ErrorAlert, PostCard};
use crate::models::*;

/// Live `IntersectionObserver` plus the closure feeding it. Kept alive together
/// so the effect destructor can `disconnect()` and drop both.
type IntersectionObserverHandle = (
    web_sys::IntersectionObserver,
    Closure<dyn FnMut(Vec<web_sys::IntersectionObserverEntry>)>,
);

/// Dimensions of the **source frame** — the picture as it will actually be
/// rendered, and therefore the only ratio the column maths may use.
///
/// Prefers `original` > `sample` > `preview`. That order matters: e621 caps
/// previews at 2:1, so for an ultra-tall post the preview is a *crop*. Charging
/// its 2.0 to the column while the card renders a 1:14 sample is what made the
/// greedy assignment optimise for heights that never land on screen, leaving a
/// multi-viewport hole under the comic's neighbours. Falls back to 4:3 when no
/// candidate has usable dimensions.
pub(crate) fn frame_dimensions(files: &crate::models::Files) -> (i64, i64) {
    for (w, h) in [
        (files.original.width, files.original.height),
        (files.sample.width, files.sample.height),
        (files.preview.width, files.preview.height),
    ] {
        if w > 1 && h > 1 {
            return (w, h);
        }
    }
    (4, 3)
}

/// Relative deviation below which a file counts as a faithful resize of the
/// frame rather than a crop. Two percent absorbs the integer rounding of small
/// thumbnails — a 150×113 preview of a 4:3 frame is 0.44% off — while still
/// rejecting a 2:1 preview of a 1:14 frame.
pub(crate) const CROP_TOLERANCE: f64 = 0.02;

/// Whether a candidate file shows the whole frame: its own aspect ratio matches
/// the frame's within [`CROP_TOLERANCE`].
pub(crate) fn is_proportional(candidate: (i64, i64), frame: (i64, i64)) -> bool {
    let (candidate_w, candidate_h) = candidate;
    let (frame_w, frame_h) = frame;
    if candidate_w <= 1 || candidate_h <= 1 || frame_w <= 1 || frame_h <= 1 {
        return false;
    }
    let candidate_ratio = candidate_h as f64 / candidate_w as f64;
    let frame_ratio = frame_h as f64 / frame_w as f64;
    ((candidate_ratio - frame_ratio) / frame_ratio).abs() <= CROP_TOLERANCE
}

/// The `height / width` ratio the card occupies, used to balance the columns.
///
/// The card renders the whole frame at its natural height, so this is the raw
/// frame ratio — never capped, and never taken from a cropped candidate.
pub(crate) fn card_media_ratio(files: &crate::models::Files) -> f64 {
    let (w, h) = frame_dimensions(files);
    h as f64 / w.max(1) as f64
}

/// Greedy masonry assignment: every item joins the column with the smallest
/// accumulated ratio, and its own ratio is then added there. Returns, per
/// column, the indices of the items it received, in input order.
///
/// Split out of `render_post_grid` so the balancing is testable without a DOM:
/// it depends only on the ratios and the column count.
fn assign_to_columns(ratios: &[f64], num_columns: usize) -> Vec<Vec<usize>> {
    let num_columns = num_columns.max(1);
    let mut heights = vec![0.0f64; num_columns];
    let mut columns: Vec<Vec<usize>> = (0..num_columns).map(|_| Vec::new()).collect();
    for (index, ratio) in ratios.iter().enumerate() {
        let shortest = heights
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(index, _)| index)
            .unwrap_or(0);
        heights[shortest] += *ratio;
        columns[shortest].push(index);
    }
    columns
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
    // Balance on the same raw ratio the card renders with, so the greedy
    // assignment matches the height that actually lands in each column: an
    // ultra-tall post leaves its column genuinely tall, and the posts after it
    // go to the shorter columns instead of piling onto the same one.
    let ratios: Vec<f64> = posts
        .iter()
        .map(|post| card_media_ratio(&post.post.files))
        .collect();
    let columns = assign_to_columns(&ratios, num_columns);

    html! {
        <div class={format!("{} m-3", effective_class)} style="align-items: start;">
            { for columns.into_iter().enumerate().map(|(column_index, column_posts)| html! {
                <div key={column_index} class="flex flex-col">
                    { for column_posts.into_iter().map(|post_index| {
                        let scored_post = &posts[post_index];
                        html! {
                        <PostCard
                            key={scored_post.post.id}
                            post={Rc::new(scored_post.post.clone())}
                            affinity={scored_post.score}
                            backend_url={backend_url.to_string()}
                            account_id={account_id}
                            session_id={session_id.to_string()}
                            position={(position_offset + post_index) as i32}
                            breakdown={scored_post.breakdown.clone()}
                            reasons={scored_post.reasons.clone()}
                            show_rating={show_rating}
                            show_affinity={show_affinity}
                            show_score={show_score}
                            show_post_number={show_post_number}
                            show_desc={show_desc}
                            show_metadata={show_metadata}
                            show_breakdown={show_breakdown}
                            show_detailed_breakdown={show_detailed_breakdown}
                        />
                        }
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

    // Set by the Retry button so the next `fetch_more` skips the
    // `is_loading`/`exhausted` guards. A plain `use_state` would not work
    // here: the callback handed to `ErrorAlert` was captured during an older
    // render and would still read the stale value. `use_mut_ref` is shared by
    // reference across every clone of the callback.
    let force_fetch = use_mut_ref(|| false);

    // Reset + trigger first fetch when the source, score cutoff or the
    // `scored` flag changes. `props.scored` used to be missing from this key,
    // so toggling "scored" left the already-loaded (unscored) list on screen
    // and only mixed scored pages in on the next infinite-scroll tick.
    {
        let reset_key = (
            props.fetch_url.clone(),
            props.score_cutoff_pct,
            props.scored,
        );
        let posts = posts.clone();
        let page = page.clone();
        let exhausted = exhausted.clone();
        let ft = fetch_trigger.clone();
        use_effect_with(reset_key, move |_| {
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
        let force_fetch = force_fetch.clone();
        Callback::from(move |_| {
            // Retry sets the flag before emitting, so neither a stuck
            // `is_loading` nor an already-`exhausted` list can swallow the
            // click and leave the Retry button permanently inert.
            let forced = std::mem::take(&mut *force_fetch.borrow_mut());
            if !forced && (*is_loading || *exhausted) {
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
                                    reasons: Vec::new(),
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

    // Retry after a failed page. Distinct from `fetch_more` on purpose: it
    // clears the error, arms `force_fetch` so the guards above are bypassed,
    // and re-emits. Used to hand `fetch_more` straight to `ErrorAlert`, which
    // made the button a no-op whenever `is_loading` or `exhausted` was set.
    let retry = {
        let fetch_more = fetch_more.clone();
        let force_fetch = force_fetch.clone();
        let error = error.clone();
        Callback::from(move |_| {
            *force_fetch.borrow_mut() = true;
            error.set(None);
            fetch_more.emit(());
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

    // Infinite-scroll sentinel observer.
    //
    // Depend on posts length so the observer re-attaches once the sentinel
    // element is actually in the DOM (after the first page loads).
    // The observer AND its callback are stored in a `use_mut_ref` so the effect
    // destructor can `disconnect()` them. The previous version called
    // `cb.forget()` + `std::mem::forget(o)` with a no-op cleanup: every
    // `posts_len` change (i.e. every infinite-scroll page) leaked one observer
    // plus one boxed JS closure holding `UseStateHandle`s, and the still-live
    // observer kept emitting `fetch_more` after the grid unmounted.
    let scroll_observer = use_mut_ref(|| None::<IntersectionObserverHandle>);
    {
        let fetch_more = fetch_more.clone();
        let sentinel = scroll_sentinel.clone();
        let posts_len = (*posts).len();
        let scroll_observer = scroll_observer.clone();
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
            match web_sys::IntersectionObserver::new(cb.as_ref().unchecked_ref()) {
                Ok(observer) => {
                    if let Some(el) = sentinel.cast::<web_sys::Element>() {
                        observer.observe(&el);
                        *scroll_observer.borrow_mut() = Some((observer, cb));
                    }
                }
                Err(e) => {
                    web_sys::console::warn_1(
                        &format!("post_grid: failed to create IntersectionObserver: {e:?}").into(),
                    );
                }
            }
            // Destructor: drop the previous observer/closure pair and stop it
            // from observing a node that is about to be replaced.
            move || {
                if let Some((observer, _cb)) = scroll_observer.borrow_mut().take() {
                    observer.disconnect();
                }
            }
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
                                    html! { <ErrorAlert message={error.clone()} on_retry={Some(retry.clone())} /> }
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

#[cfg(test)]
mod tests {
    use super::{assign_to_columns, card_media_ratio, frame_dimensions, is_proportional};
    use crate::models::{FileOriginal, FilePreview, FileSample, Files};

    fn files_with_preview(width: i64, height: i64) -> Files {
        Files {
            preview: FilePreview {
                width,
                height,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// A 1:18.7 post exactly as e621 serves it: a full-frame sample/original and
    /// a preview capped at 2:1.
    fn tall_post_files() -> Files {
        Files {
            original: FileOriginal {
                width: 91,
                height: 1700,
                url: None,
            },
            sample: FileSample {
                width: 91,
                height: 1700,
                ..Default::default()
            },
            preview: FilePreview {
                width: 256,
                height: 512,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn media_ratio_is_not_capped() {
        // A long comic strip: the estimator must report the whole 1:50, because
        // the card renders it at that height — nothing is cropped any more.
        let ratio = card_media_ratio(&files_with_preview(100, 5000));
        assert!((ratio - 50.0).abs() < f64::EPSILON, "got {ratio}");
        // A normal 4:3 preview passes through untouched.
        let ratio = card_media_ratio(&files_with_preview(400, 300));
        assert!((ratio - 0.75).abs() < 1e-9, "got {ratio}");
        // Unknown dimensions fall back to 4:3 instead of dividing by zero.
        let ratio = card_media_ratio(&files_with_preview(0, 0));
        assert!((ratio - 0.75).abs() < 1e-9, "got {ratio}");
    }

    #[test]
    fn frame_dimensions_prefer_the_uncropped_source() {
        // The preview is a 2:1 crop of a 1:18.7 frame — the column must be
        // charged 18.7, not 2.0, or the posts after the comic pile into a
        // column that is already a dozen viewports tall.
        let files = tall_post_files();
        assert_eq!(frame_dimensions(&files), (91, 1700));
        let ratio = card_media_ratio(&files);
        assert!((ratio - 18.68).abs() < 0.01, "got {ratio}");
    }

    #[test]
    fn proportional_check_rejects_a_capped_preview() {
        let frame = (91, 1700);
        assert!(is_proportional((91, 1700), frame), "the frame itself");
        assert!(
            is_proportional((455, 8500), frame),
            "a faithful downscale is still the whole frame"
        );
        assert!(!is_proportional((256, 512), frame), "2:1 preview of 1:18.7");
        // Thumbnail rounding must not disqualify an honest resize.
        assert!(is_proportional((150, 113), (1920, 1440)));
        // Unknown dimensions are never treated as proportional.
        assert!(!is_proportional((0, 0), frame));
        assert!(!is_proportional((100, 100), (0, 0)));
    }

    #[test]
    fn order_within_a_column_follows_the_feed() {
        // Positions inside a column must stay in input order: the grid is a
        // feed, and the greedy pass must not reshuffle it the way a
        // longest-first balancing pass would.
        let columns = assign_to_columns(&[1.0, 9.0, 1.0, 1.0, 1.0], 2);
        for column in &columns {
            let mut sorted = column.clone();
            sorted.sort_unstable();
            assert_eq!(*column, sorted, "column {column:?} is out of order");
        }
    }

    #[test]
    fn ultra_tall_post_stands_alone_in_its_column() {
        // One 1:50 post plus three normal ones over two columns: the tall post
        // must be charged its full height, so the following posts go to the
        // other column instead of stacking under it and leaving the neighbour a
        // viewport-sized hole.
        let normal = 3.0 / 4.0;
        let columns = assign_to_columns(&[normal, 50.0, normal, normal], 2);
        assert_eq!(columns[0], vec![0, 2, 3]);
        assert_eq!(columns[1], vec![1]);
    }

    #[test]
    fn capped_weight_would_pick_a_different_column() {
        // Distinguishes the raw ratio from the old `MAX_CARD_MEDIA_RATIO = 2.5`
        // cap: with a capped weight the third post fits "under" the tall one
        // (2.5 < 4.0) and lands in its column; with the true height it does not.
        let columns = assign_to_columns(&[4.0, 50.0, 1.0], 2);
        assert_eq!(columns, vec![vec![0, 2], vec![1]]);
    }

    #[test]
    fn every_post_is_placed_exactly_once() {
        let columns = assign_to_columns(&[1.0, 2.0, 3.0, 4.0, 5.0], 3);
        let mut placed: Vec<usize> = columns.iter().flatten().copied().collect();
        placed.sort_unstable();
        assert_eq!(placed, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn zero_columns_still_yields_a_column() {
        assert_eq!(assign_to_columns(&[1.0], 0), vec![vec![0]]);
    }
}
