//! Reusable loading placeholders for non-grid sections.
//!
//! `render_post_grid_skeleton` in `post_grid.rs` covers post grids. These
//! cover standalone card sections (taste profile, history list, …) so a
//! first load reserves real height with skeleton blocks instead of
//! collapsing to a bare spinner.

use yew::prelude::*;

/// Skeleton line-group placed inside a section card's body while its first
/// payload loads (used by `TasteProfileCard`). The surrounding card shell
/// keeps the layout height stable.
#[function_component(SkeletonLines)]
pub fn skeleton_lines() -> Html {
    html! {
        <div class="space-y-3" aria-hidden="true">
            <div class="skeleton h-4 w-full"></div>
            <div class="skeleton h-4 w-5/6"></div>
            <div class="skeleton h-4 w-2/3"></div>
        </div>
    }
}
