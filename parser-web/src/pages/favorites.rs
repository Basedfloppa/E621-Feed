use yew::prelude::*;

use crate::components::*;
use crate::models::*;
use crate::pages::UserInfo;

/// Grid layout options (mirrors feed.rs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FavGrid {
    Auto,
    Three,
    Two,
    One,
}
impl FavGrid {
    fn from_storage(s: Option<String>) -> Self {
        match s.as_deref() {
            Some("3") => Self::Three,
            Some("2") => Self::Two,
            Some("1") => Self::One,
            _ => Self::Auto,
        }
    }
    fn grid_class(self) -> &'static str {
        match self {
            Self::Auto => {
                "grid grid-cols-1 sm:grid-cols-2 md:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5 gap-3"
            }
            Self::Three => "grid grid-cols-2 sm:grid-cols-3 gap-3",
            Self::Two => "grid grid-cols-2 gap-3",
            Self::One => "grid grid-cols-1 gap-3",
        }
    }
}

/// Read a display setting from unified settings_show_* key, falling back
/// to an old per-page key for backward compatibility.
pub fn read_display_setting(suffix: &str, old: &str, default: bool) -> bool {
    let new_key = format!("settings_show_{}", suffix);
    let storage = || web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    storage()
        .and_then(|s| s.get_item(&new_key).ok().flatten())
        .or_else(|| storage().and_then(|s| s.get_item(old).ok().flatten()))
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(default)
}

/// Favorites page — shows the selected user's favourited posts from e621.
#[function_component(FavoritesPage)]
pub fn favorites_page() -> Html {
    let _settings_tick = use_settings_tick();
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let is_loading = use_state(|| false);
    let grid = {
        let storage = || web_sys::window().and_then(|w| w.local_storage().ok().flatten());
        FavGrid::from_storage(
            storage()
                .and_then(|s| s.get_item("settings_grid_type").ok().flatten())
                .or_else(|| {
                    storage().and_then(|s| s.get_item("favorites_grid_type").ok().flatten())
                }),
        )
    };
    // Compute fetch URL and card context from the same backend configuration.
    let backend_url = read_config_from_head()
        .map(|cfg| cfg.backend_domain)
        .unwrap_or_default();
    let fetch_url = selected_user
        .as_ref()
        .map(|u| format!("{}/browse/favorites/{}", backend_url, u.id))
        .unwrap_or_default();

    // Display settings
    let show_rating = read_display_setting("rating", "favorites_show_rating", true);
    let show_affinity = read_display_setting("affinity", "favorites_show_affinity", false);
    let show_score = read_display_setting("score", "favorites_show_score", true);
    let show_post_number = read_display_setting("post_number", "favorites_show_post_number", true);
    let show_desc = read_display_setting("desc", "favorites_show_desc", true);
    let show_metadata = read_display_setting("metadata", "favorites_show_metadata", false);
    let show_breakdown = read_display_setting("breakdown", "favorites_show_breakdown", false);
    let show_detailed_breakdown = read_display_setting(
        "show_detailed_breakdown",
        "favorites_show_detailed_breakdown",
        false,
    );

    html! {
        <div class="m-4 gap-2">
            <h1 class="text-2xl font-semibold text-base-content mb-3">{ "Favorites" }</h1>
            <div class="flex flex-wrap gap-3 items-center mb-3">
                <div>
                    <SavedAccountsSelect
                        selected_user={selected_user.clone()}
                        is_loading={is_loading.clone()}
                    />
                </div>
            </div>

            if selected_user.is_some() && !fetch_url.is_empty() {
                <PostGrid
                    grid_class={grid.grid_class().to_string()}
                    fetch_url={fetch_url.clone()}
                    scored=false
                    backend_url={backend_url.clone()}
                    account_id={selected_user.as_ref().map(|u| u.id as i32).unwrap_or_default()}
                    show_rating={show_rating}
                    show_affinity={show_affinity}
                    show_score={show_score}
                    show_post_number={show_post_number}
                    show_desc={show_desc}
                    show_metadata={show_metadata}
                    show_breakdown={show_breakdown}
                    show_detailed_breakdown={show_detailed_breakdown}
                    empty_message={"Select an account to see your favourites."}
                />
            } else {
                <p class="text-base-content/70 text-center my-8">{
                    if selected_user.is_some() { "Loading configuration..." } else { "Select an account to see your favourites." }
                }</p>
            }
        </div>
    }
}
