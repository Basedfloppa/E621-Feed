use yew::prelude::*;

use crate::components::*;
use crate::models::*;
use crate::pages::UserInfo;

/// Grid layout options (mirrors feed.rs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TrendingGrid {
    Auto,
    Three,
    Two,
    One,
}
impl TrendingGrid {
    fn from_storage(s: Option<String>) -> Self {
        match s.as_deref() {
            Some("3") => Self::Three,
            Some("2") => Self::Two,
            Some("1") => Self::One,
            _ => Self::Auto,
        }
    }
    fn to_storage(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Three => "3",
            Self::Two => "2",
            Self::One => "1",
        }
    }
    fn grid_class(self) -> &'static str {
        match self {
            Self::Auto => {
                "grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-3"
            }
            Self::Three => "grid grid-cols-3 gap-3",
            Self::Two => "grid grid-cols-2 gap-3",
            Self::One => "grid grid-cols-1 gap-3",
        }
    }
}

/// Trending page — shows popular posts from e621 (order:hot).
#[function_component(TrendingPage)]
pub fn trending_page() -> Html {
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let is_loading = use_state(|| false);
    let grid = use_state(|| {
        TrendingGrid::from_storage(
            web_sys::window()
                .and_then(|w| w.local_storage().ok().flatten())
                .and_then(|s| s.get_item("trending_grid_type").ok().flatten()),
        )
    });
    // Persist grid
    {
        let g = *grid;
        use_effect_with(g, move |g| {
            if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = s.set_item("trending_grid_type", g.to_storage());
            }
            || ()
        });
    }
    // Compute fetch URL from selected user (recomputed on every render).
    let fetch_url = {
        let cfg = read_config_from_head();
        selected_user
            .as_ref()
            .and_then(|u| {
                cfg.as_ref()
                    .map(|c| format!("{}/browse/trending/{}", c.backend_domain, u.id))
            })
            .unwrap_or_default()
    };

    // Display settings.
    let show_rating = use_state(|| true);
    let show_affinity = use_state(|| false);
    let show_score = use_state(|| true);
    let show_post_number = use_state(|| true);
    let show_desc = use_state(|| true);
    let show_metadata = use_state(|| false);
    let show_breakdown = use_state(|| false);

    html! {
        <div class="m-4 gap-2">
            <h1 class="text-2xl font-semibold text-base-content mb-3">{ "Trending" }</h1>
            <div class="flex flex-wrap gap-3 items-center mb-3">
                <div>
                    <SavedAccountsSelect
                        selected_user={selected_user.clone()}
                        is_loading={is_loading.clone()}
                    />
                </div>
                <div>
                    <details class="dropdown dropdown-end">
                        <summary class="btn btn-outline"><IconSliders />{ " Display" }</summary>
                        <div class="menu dropdown-content p-3 shadow bg-base-100 rounded-box w-72 z-50" style="min-width:260px;">
                            <span class="text-xs text-base-content/70 block mb-1">{ "Badges" }</span>
                            <label class="label cursor-pointer py-1"><span class="text-base-content">{"Rating badge"}</span>
                                <input type="checkbox" class="toggle toggle-sm" checked={*show_rating}
                                    onchange={{let s=show_rating.clone(); Callback::from(move |_: Event| s.set(!*s))}} /></label>
                            <label class="label cursor-pointer py-1"><span class="text-base-content">{"Affinity score"}</span>
                                <input type="checkbox" class="toggle toggle-sm" checked={*show_affinity}
                                    onchange={{let s=show_affinity.clone(); Callback::from(move |_: Event| s.set(!*s))}} /></label>
                            <label class="label cursor-pointer py-1"><span class="text-base-content">{"Post score"}</span>
                                <input type="checkbox" class="toggle toggle-sm" checked={*show_score}
                                    onchange={{let s=show_score.clone(); Callback::from(move |_: Event| s.set(!*s))}} /></label>
                            <label class="label cursor-pointer py-1"><span class="text-base-content">{"Post number"}</span>
                                <input type="checkbox" class="toggle toggle-sm" checked={*show_post_number}
                                    onchange={{let s=show_post_number.clone(); Callback::from(move |_: Event| s.set(!*s))}} /></label>
                            <div class="divider my-1"></div>
                            <span class="text-xs text-base-content/70 block mb-1">{ "Cards" }</span>
                            <label class="label cursor-pointer py-1"><span class="text-base-content">{"Post text / tags"}</span>
                                <input type="checkbox" class="toggle toggle-sm" checked={*show_desc}
                                    onchange={{let s=show_desc.clone(); Callback::from(move |_: Event| s.set(!*s))}} /></label>
                            <label class="label cursor-pointer py-1"><span class="text-base-content">{"File metadata"}</span>
                                <input type="checkbox" class="toggle toggle-sm" checked={*show_metadata}
                                    onchange={{let s=show_metadata.clone(); Callback::from(move |_: Event| s.set(!*s))}} /></label>
                            <label class="label cursor-pointer py-1"><span class="text-base-content">{"Score breakdown"}</span>
                                <input type="checkbox" class="toggle toggle-sm" checked={*show_breakdown}
                                    onchange={{let s=show_breakdown.clone(); Callback::from(move |_: Event| s.set(!*s))}} /></label>
                        </div>
                    </details>
                </div>
                <div class="feed-grid-col">
                    <span class="block text-xs text-base-content/70 mb-1">{ "Grid" }</span>
                    <div class="join" role="group" aria-label="Grid type">
                        <button type="button" class={classes!("btn", "btn-outline", "btn-sm", if *grid == TrendingGrid::Auto { "btn-active" } else { "" })}
                            aria-label="Auto grid" title="Auto grid"
                            onclick={{let g=grid.clone(); Callback::from(move |_| g.set(TrendingGrid::Auto))}}><IconWater /></button>
                        <button type="button" class={classes!("btn", "btn-outline", "btn-sm", if *grid == TrendingGrid::Three { "btn-active" } else { "" })}
                            aria-label="Three columns" title="Three columns"
                            onclick={{let g=grid.clone(); Callback::from(move |_| g.set(TrendingGrid::Three))}}><IconGrid3x3 /></button>
                        <button type="button" class={classes!("btn", "btn-outline", "btn-sm", if *grid == TrendingGrid::Two { "btn-active" } else { "" })}
                            aria-label="Two columns" title="Two columns"
                            onclick={{let g=grid.clone(); Callback::from(move |_| g.set(TrendingGrid::Two))}}><IconGridFill /></button>
                        <button type="button" class={classes!("btn", "btn-outline", "btn-sm", if *grid == TrendingGrid::One { "btn-active" } else { "" })}
                            aria-label="Single column" title="Single column"
                            onclick={{let g=grid.clone(); Callback::from(move |_| g.set(TrendingGrid::One))}}><IconSquareFill /></button>
                    </div>
                </div>
            </div>

            if selected_user.is_some() && !fetch_url.is_empty() {
                <PostGrid
                    grid_class={(*grid).grid_class().to_string()}
                    fetch_url={fetch_url.clone()}
                    scored=false
                    show_rating={show_rating}
                    show_affinity={show_affinity}
                    show_score={show_score}
                    show_post_number={show_post_number}
                    show_desc={show_desc}
                    show_metadata={show_metadata}
                    show_breakdown={show_breakdown}
                    empty_message={"Select an account to see trending posts."}
                />
            } else {
                <p class="text-base-content/70 text-center my-8">{
                    if selected_user.is_some() { "Loading configuration..." } else { "Select an account to see trending posts." }
                }</p>
            }
        </div>
    }
}
