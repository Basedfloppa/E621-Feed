use serde::{Deserialize, Serialize};
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::components::*;
use crate::models::*;
use crate::pages::UserInfo;

const KEY_GROUPINGS: &str = "catalog_groupings";

/// Read a display setting from the unified `settings_show_*` key, falling back
/// to an old per-page key for backward compatibility (same helper the other
/// pages use).
fn read_display_setting(suffix: &str, old: &str, default: bool) -> bool {
    let new_key = format!("settings_show_{}", suffix);
    let storage = || web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    storage()
        .and_then(|s| s.get_item(&new_key).ok().flatten())
        .or_else(|| storage().and_then(|s| s.get_item(old).ok().flatten()))
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(default)
}

/// Fetch local tag suggestions (from the catalog's saved posts) for the last
/// whitespace-separated word of `value`, filling the given suggestion state.
/// Clears the list when the word is empty. Shared by the search box and the
/// grouping tag-query input.
fn spawn_tag_suggest(
    backend_url: &str,
    account_id: i32,
    value: &str,
    set_suggestions: UseStateHandle<Vec<String>>,
    set_loading: UseStateHandle<bool>,
    set_error: UseStateHandle<Option<String>>,
) {
    let tag = value.split_whitespace().last().unwrap_or("").to_string();
    if tag.is_empty() {
        set_suggestions.set(Vec::new());
        set_error.set(None);
        set_loading.set(false);
        return;
    }
    set_loading.set(true);
    set_error.set(None);
    let url = format!(
        "{}/catalog/{}/tag/suggest?prefix={}",
        backend_url,
        account_id,
        urlencoding::encode(&tag)
    );
    let suggestions = set_suggestions.clone();
    let suggestions_loading = set_loading.clone();
    let suggestions_error = set_error.clone();
    spawn_local(async move {
        match api_get(&url).send().await {
            Ok(response) if response.ok() => match response.json::<Vec<String>>().await {
                Ok(mut values) => {
                    values.sort();
                    values.dedup();
                    suggestions.set(values);
                }
                Err(_) => suggestions_error.set(Some(
                    "Tag suggestions could not be read. Try again.".to_string(),
                )),
            },
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                suggestions_error.set(Some(humanize_error_body(status, &body)));
            }
            Err(error) => suggestions_error.set(Some(humanize_network_error(error))),
        }
        suggestions_loading.set(false);
    });
}

/// Grid layout options (mirrors feed/favorites/trending). Read from the
/// unified `settings_grid_type` setting so the catalog follows the same
/// layout preference as every other page.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CatGrid {
    Auto,
    Three,
    Two,
    One,
}
impl CatGrid {
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

/// A user-defined, named tag grouping: a curated e621 tag query the catalog
/// search box can re-run with one click. Persisted in localStorage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SavedGroup {
    name: String,
    query: String,
}

/// Human-readable byte size (e.g. `2.1 GB`) for the queue status.
fn human_bytes(n: i64) -> String {
    let n = n.max(0) as f64;
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut i = 0;
    let mut v = n;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", v, units[i])
}

fn read_groupings() -> Vec<SavedGroup> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(KEY_GROUPINGS).ok().flatten())
        .and_then(|raw| serde_json::from_str::<Vec<SavedGroup>>(&raw).ok())
        .unwrap_or_default()
}

fn write_groupings(groups: &[SavedGroup]) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten())
        && let Ok(raw) = serde_json::to_string(groups)
    {
        let _ = storage.set_item(KEY_GROUPINGS, &raw);
    }
}

/// Local catalog browser: the owner's saved posts, with tag search (same tag
/// alias suggestions as the Search page) and user-defined named groupings.
///
/// Media is stored flat in the `media/` folder (no per-tag subfolders). This
/// page searches the locally-saved catalog rather than showing a tag cloud.
#[function_component(CatalogPage)]
pub fn catalog_page() -> Html {
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let is_loading = use_state(|| false);
    let error = use_state(|| Option::<String>::None);
    let disabled = use_state(|| false);
    let tick = use_state(|| 0u32);

    // Media-queue state.
    let queue_status = use_state(|| Option::<MediaQueueStatus>::None);
    let queue_tick = use_state(|| 0u32);

    // Tag search state (mirrors the Search page).
    let search_input = use_state(String::new);
    let submitted_query = use_state(String::new);
    let suggestions = use_state(Vec::<String>::new);
    let suggestions_loading = use_state(|| false);
    let suggestions_error = use_state(|| Option::<String>::None);
    // Tag suggestions for the grouping tag-query input.
    let group_suggestions = use_state(Vec::<String>::new);
    let group_suggestions_loading = use_state(|| false);
    let group_suggestions_error = use_state(|| Option::<String>::None);

    // Named groupings (user-defined saved tag searches).
    let groupings = use_state(read_groupings);
    let group_name = use_state(String::new);
    let group_query = use_state(String::new);

    let backend_url = read_config_from_head()
        .map(|c| c.backend_domain)
        .unwrap_or_default();

    // Detect whether the catalog is enabled (404 when it isn't). Reuses the
    // media-status probe, which is gated by the same `catalog_enabled()`.
    {
        let selected_user = selected_user.clone();
        let disabled = disabled.clone();
        let t = *tick;
        use_effect_with((selected_user, t), move |(user, _)| {
            let Some(user) = user.as_ref() else { return };
            let Some(cfg) = read_config_from_head() else {
                return;
            };
            let url = format!("{}/catalog/{}/media/status", cfg.backend_domain, user.id);
            let disabled = disabled.clone();
            spawn_local(async move {
                if let Ok(resp) = api_get(&url).send().await {
                    if resp.status() == 404 {
                        disabled.set(true);
                    } else {
                        disabled.set(false);
                    }
                }
            });
        });
    }

    let on_retry = {
        let tick = tick.clone();
        Callback::from(move |_| tick.set(*tick + 1))
    };

    // Fetch media-queue status whenever the selected user or queue_tick changes.
    {
        let selected_user = selected_user.clone();
        let queue_status = queue_status.clone();
        let t = *queue_tick;
        use_effect_with((selected_user, t), move |(user, _)| {
            let Some(user) = user.as_ref() else { return };
            let Some(cfg) = read_config_from_head() else {
                return;
            };
            let url = format!("{}/catalog/{}/media/status", cfg.backend_domain, user.id);
            let qs = queue_status.clone();
            spawn_local(async move {
                if let Ok(resp) = api_get(&url).send().await
                    && resp.ok()
                    && let Ok(s) = resp.json::<MediaQueueStatus>().await
                {
                    qs.set(Some(s));
                }
            });
        });
    }

    // Tag suggestions for the search box, sourced from the LOCAL DB
    // (no e621 round-trip): the account's saved posts' tags matching the
    // current word, ordered by frequency.
    {
        let input_value = (*search_input).clone();
        let user = selected_user.clone();
        let backend_url = backend_url.clone();
        let suggestions = suggestions.clone();
        let suggestions_loading = suggestions_loading.clone();
        let suggestions_error = suggestions_error.clone();
        use_effect_with((input_value, user), move |(value, user)| {
            if let Some(user) = user.as_ref() {
                if !backend_url.is_empty() {
                    spawn_tag_suggest(
                        &backend_url,
                        user.id as i32,
                        value,
                        suggestions.clone(),
                        suggestions_loading.clone(),
                        suggestions_error.clone(),
                    );
                } else {
                    suggestions.set(Vec::new());
                    suggestions_loading.set(false);
                }
            } else {
                suggestions.set(Vec::new());
                suggestions_loading.set(false);
            }
            || ()
        });
    }

    // Tag suggestions for the grouping tag-query input (same local source).
    {
        let input_value = (*group_query).clone();
        let user = selected_user.clone();
        let backend_url = backend_url.clone();
        let group_suggestions = group_suggestions.clone();
        let group_suggestions_loading = group_suggestions_loading.clone();
        let group_suggestions_error = group_suggestions_error.clone();
        use_effect_with((input_value, user), move |(value, user)| {
            if let Some(user) = user.as_ref() {
                if !backend_url.is_empty() {
                    spawn_tag_suggest(
                        &backend_url,
                        user.id as i32,
                        value,
                        group_suggestions.clone(),
                        group_suggestions_loading.clone(),
                        group_suggestions_error.clone(),
                    );
                } else {
                    group_suggestions.set(Vec::new());
                    group_suggestions_loading.set(false);
                }
            } else {
                group_suggestions.set(Vec::new());
                group_suggestions_loading.set(false);
            }
            || ()
        });
    }

    let backend_url_for_grid = backend_url;

    // Search input — reattach an alias selection to the existing query.
    let on_search_input = {
        let search_input = search_input.clone();
        let suggestions = suggestions.clone();
        Callback::from(move |e: InputEvent| {
            let value = e.target_unchecked_into::<HtmlInputElement>().value();
            let previous = (*search_input).clone();
            if suggestions.iter().any(|s| s == &value) && previous.split_whitespace().count() > 1 {
                let prefix = previous
                    .rsplit_once(char::is_whitespace)
                    .map(|(prefix, _)| format!("{prefix} "))
                    .unwrap_or_default();
                search_input.set(format!("{prefix}{value}"));
            } else {
                search_input.set(value);
            }
        })
    };
    let on_submit = {
        let search_input = search_input.clone();
        let submitted_query = submitted_query.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            submitted_query.set((*search_input).trim().to_string());
        })
    };

    // --- Named groupings ---
    let on_group_name_change = {
        let group_name = group_name.clone();
        Callback::from(move |e: InputEvent| {
            group_name.set(e.target_unchecked_into::<HtmlInputElement>().value());
        })
    };
    let on_group_query_change = {
        let group_query = group_query.clone();
        let group_suggestions = group_suggestions.clone();
        Callback::from(move |e: InputEvent| {
            let value = e.target_unchecked_into::<HtmlInputElement>().value();
            let previous = (*group_query).clone();
            // Reattach a datalist selection to the existing query so picking
            // an autocompleted tag never discards earlier words.
            if group_suggestions.iter().any(|s| s == &value)
                && previous.split_whitespace().count() > 1
            {
                let prefix = previous
                    .rsplit_once(char::is_whitespace)
                    .map(|(prefix, _)| format!("{prefix} "))
                    .unwrap_or_default();
                group_query.set(format!("{prefix}{value}"));
            } else {
                group_query.set(value);
            }
        })
    };
    let on_save_group = {
        let groupings = groupings.clone();
        let group_name = group_name.clone();
        let group_query = group_query.clone();
        Callback::from(move |_: MouseEvent| {
            let name = (*group_name).trim().to_string();
            let query = (*group_query).trim().to_string();
            if name.is_empty() || query.is_empty() {
                return;
            }
            let mut list: Vec<SavedGroup> = (*groupings).clone();
            list.retain(|g| g.name != name);
            list.push(SavedGroup {
                name: name.clone(),
                query: query.clone(),
            });
            write_groupings(&list);
            groupings.set(list);
            group_name.set(String::new());
            group_query.set(String::new());
        })
    };
    let on_run_group = {
        let search_input = search_input.clone();
        let submitted_query = submitted_query.clone();
        Callback::from(move |query: String| {
            search_input.set(query.clone());
            submitted_query.set(query);
        })
    };
    let on_remove_group = {
        let groupings = groupings.clone();
        Callback::from(move |name: String| {
            let mut list: Vec<SavedGroup> = (*groupings).clone();
            list.retain(|g| g.name != name);
            write_groupings(&list);
            groupings.set(list);
        })
    };

    // --- Media queue control ---
    let refresh_all = {
        let tick = tick.clone();
        let queue_tick = queue_tick.clone();
        Callback::from(move |_| {
            tick.set(*tick + 1);
            queue_tick.set(*queue_tick + 1);
        })
    };
    let queue_cmd = {
        let selected_user = selected_user.clone();
        let refresh_all = refresh_all.clone();
        Callback::from(move |cmd: &'static str| {
            let Some(cfg) = read_config_from_head() else {
                return;
            };
            let Some(u) = selected_user.as_ref() else {
                return;
            };
            let url = format!("{}/catalog/{}/media/{cmd}", cfg.backend_domain, u.id);
            spawn_local(async move {
                let _ = api_post(&url).send().await;
            });
            refresh_all.emit(());
        })
    };
    let on_pause = {
        let q = queue_cmd.clone();
        Callback::from(move |_| q.emit("pause"))
    };
    let on_resume = {
        let q = queue_cmd.clone();
        Callback::from(move |_| q.emit("resume"))
    };
    let on_kick = {
        let q = queue_cmd.clone();
        Callback::from(move |_| q.emit("kick"))
    };
    // Remove the entire local media cache: deletes the on-disk originals and
    // wipes the media index (the links to local files). Confirmed first — it
    // resets the offline cache and saved posts re-download on the next pass.
    let on_clear_cache = {
        let selected_user = selected_user.clone();
        let refresh_all = refresh_all.clone();
        Callback::from(move |_| {
            let confirmed = web_sys::window()
                .and_then(|w| {
                    w.confirm_with_message(
                        "Delete all locally cached media files and their index? \
                         Saved posts will re-download on the next worker pass.",
                    )
                    .ok()
                })
                .unwrap_or(false);
            if !confirmed {
                return;
            }
            let Some(cfg) = read_config_from_head() else {
                return;
            };
            let Some(u) = selected_user.as_ref() else {
                return;
            };
            let url = format!("{}/catalog/{}/media", cfg.backend_domain, u.id);
            spawn_local(async move {
                let _ = api_delete(&url).send().await;
            });
            refresh_all.emit(());
        })
    };

    // Display settings follow the same unified settings (settings_show_*) as
    // every other page, so catalog cards look identical to search/favorites.
    let _settings_tick = use_settings_tick();
    // Grid layout follows the unified `settings_grid_type` setting (same as
    // feed/favorites/trending), falling back to a legacy per-page key.
    let grid = {
        let storage = || web_sys::window().and_then(|w| w.local_storage().ok().flatten());
        CatGrid::from_storage(
            storage()
                .and_then(|s| s.get_item("settings_grid_type").ok().flatten())
                .or_else(|| storage().and_then(|s| s.get_item("catalog_grid_type").ok().flatten())),
        )
    };
    let show_rating = read_display_setting("rating", "catalog_show_rating", true);
    let show_affinity = read_display_setting("affinity", "catalog_show_affinity", false);
    let show_score = read_display_setting("score", "catalog_show_score", true);
    let show_post_number = read_display_setting("post_number", "catalog_show_post_number", true);
    let show_desc = read_display_setting("desc", "catalog_show_desc", true);
    let show_metadata = read_display_setting("metadata", "catalog_show_metadata", false);
    let show_breakdown = read_display_setting("breakdown", "catalog_show_breakdown", false);
    let show_detailed_breakdown = read_display_setting(
        "show_detailed_breakdown",
        "catalog_show_detailed_breakdown",
        false,
    );

    // The grid URL: search the locally-saved catalog by the tag query (empty
    // query = all saved posts). PostGrid handles pagination via ?page=.
    let fetch_url = selected_user.as_ref().map(|u| {
        format!(
            "{}/catalog/{}/search?query={}",
            backend_url_for_grid,
            u.id,
            urlencoding::encode(submitted_query.trim())
        )
    });

    html! {
        <div class="m-4">
            <h1 class="text-2xl font-semibold text-base-content mb-3">{ "Local Catalog" }</h1>
            <p class="text-sm text-base-content/70 mb-3">
                { "Your saved posts, searchable by tag. Enable \"save favourites\" in the server config to populate this. Media is stored flat in the media/ folder (no per-tag subfolders)." }
            </p>

            // Unified toolbar: account selector + media queue + tag search +
            // grouping creation, in one compact panel.
            <div class="card bg-base-100 border border-base-300 mb-4">
                <div class="card-body p-3 gap-2">
                    <div class="flex flex-wrap items-center gap-3">
                        <SavedAccountsSelect selected_user={selected_user.clone()} is_loading={is_loading.clone()} />
                        if selected_user.is_some() && !*disabled {
                            <span class="text-sm">
                                {
                                    if let Some(q) = queue_status.as_ref() {
                                        format!("Pending: {} · Stored: {} · {} on disk", q.pending, q.stored, human_bytes(q.bytes))
                                    } else {
                                        "Loading media queue…".to_string()
                                    }
                                }
                            </span>
                            {
                                if let Some(q) = queue_status.as_ref() {
                                    if q.paused {
                                        html! { <button type="button" class="btn btn-sm btn-primary" onclick={on_resume}>{ "Resume" }</button> }
                                    } else {
                                        html! { <button type="button" class="btn btn-sm" onclick={on_pause}>{ "Pause" }</button> }
                                    }
                                } else {
                                    html! {}
                                }
                            }
                            <button type="button" class="btn btn-sm btn-outline" onclick={on_kick}>{ "Run now" }</button>
                            <button type="button" class="btn btn-sm btn-outline btn-error" onclick={on_clear_cache}>{ "Clear cache" }</button>
                        }
                    </div>
                    if selected_user.is_some() && !*disabled {
                        <div class="flex flex-wrap items-center gap-2">
                            <form class="flex-1 min-w-56 max-w-xl" onsubmit={on_submit}>
                                <div class="join w-full">
                                    <input
                                        id="catalog-search-query"
                                        class="input input-sm join-item w-full"
                                        type="search"
                                        list="catalog-tag-suggestions"
                                        value={(*search_input).clone()}
                                        oninput={on_search_input}
                                        placeholder="for example: wolf rating:s"
                                        aria-describedby="catalog-tag-suggestion-status"
                                    />
                                    <button class="btn btn-sm btn-primary join-item" type="submit">{ "Search" }</button>
                                </div>
                            </form>
                            <datalist id="catalog-tag-suggestions">
                                { for suggestions.iter().map(|tag| html! { <option value={tag.clone()} /> }) }
                            </datalist>
                            <input class="input input-sm input-bordered w-36" placeholder="group name"
                                value={(*group_name).clone()} oninput={on_group_name_change} />
                            <input class="input input-sm input-bordered w-48" placeholder="tag query, e.g. fox dragon"
                                list="catalog-group-tag-suggestions"
                                value={(*group_query).clone()} oninput={on_group_query_change} />
                            <datalist id="catalog-group-tag-suggestions">
                                { for group_suggestions.iter().map(|tag| html! { <option value={tag.clone()} /> }) }
                            </datalist>
                            <button type="button" class="btn btn-sm btn-primary" onclick={on_save_group}>
                                { "Create grouping" }
                            </button>
                        </div>
                        <p id="catalog-tag-suggestion-status" class="text-xs text-base-content/70" aria-live="polite">
                            {
                                if *suggestions_loading { "Loading tag aliases…" }
                                else if let Some(error) = &*suggestions_error { error.as_str() }
                                else if !search_input.trim().is_empty() && suggestions.is_empty() { "No tag aliases found." }
                                else { "" }
                            }
                        </p>
                        if !groupings.is_empty() {
                            <div class="flex flex-wrap gap-2 mt-1">
                                { for groupings.iter().map(|g| {
                                    let name = g.name.clone();
                                    let query = g.query.clone();
                                    let on_run = on_run_group.clone();
                                    let on_remove = on_remove_group.clone();
                                    html! {
                                        <div class="join">
                                            <button type="button" class="btn btn-sm btn-outline join-item"
                                                title={format!("Run: {}", g.query)}
                                                onclick={move |_: MouseEvent| on_run.emit(query.clone())}>
                                                { format!("{}", g.name) }
                                            </button>
                                            <button type="button" class="btn btn-sm btn-outline btn-error join-item"
                                                title="Remove grouping"
                                                onclick={move |_: MouseEvent| on_remove.emit(name.clone())}>
                                                { "✕" }
                                            </button>
                                        </div>
                                    }
                                }) }
                            </div>
                        } else {
                            <p class="text-xs text-base-content/60">{ "No groupings yet. Name a tag query above and click “Create grouping” to add a one-click filter." }</p>
                        }
                    }
                </div>
            </div>

            if *disabled {
                <p class="text-base-content/70 text-center my-8">
                    { "The local catalog is disabled on this server. Enable catalog.save_favourites or catalog.save_all in config.toml to use it." }
                </p>
            } else if let Some(err) = error.as_ref() {
                <ErrorAlert message={err.clone()} on_retry={on_retry} />
            } else if selected_user.is_some() {
                <>

                    if let Some(url) = fetch_url {
                        <PostGrid
                            grid_class={grid.grid_class().to_string()}
                            fetch_url={url}
                            scored=false
                            backend_url={backend_url_for_grid.clone()}
                            account_id={selected_user.as_ref().map(|u| u.id as i32).unwrap_or_default()}
                            show_rating={show_rating}
                            show_score={show_score}
                            show_post_number={show_post_number}
                            show_desc={show_desc}
                            show_affinity={show_affinity}
                            show_metadata={show_metadata}
                            show_breakdown={show_breakdown}
                            show_detailed_breakdown={show_detailed_breakdown}
                            empty_message={
                                if submitted_query.trim().is_empty() {
                                    "No saved posts yet. Add favourites to build a local catalog."
                                } else {
                                    "No saved posts matched this query."
                                }
                            }
                        />
                    }
                </>
            } else {
                <p class="text-base-content/70 text-center my-8">{ "Select an account to browse the local catalog." }</p>
            }
        </div>
    }
}
